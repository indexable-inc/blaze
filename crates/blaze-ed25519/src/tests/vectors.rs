//! Verification against RFC 8032 section 7.1's vectors and blaze's own edge cases
//! (`tests/data/mod.rs` says where each byte comes from), the eight points of small order, the
//! keys of mixed order, S's range at its boundaries, and the base point's order.

use std::vec;
use std::vec::Vec;

use crate::point::{BASEPOINT, BASEPOINT_BYTES, Point, double_scalar_mul_basepoint};
use crate::{Encoding, Refused, VerifyingKey, scalar, sha512, verify, verify_parts};

use super::data::{self, EDGE, MIXED_KEYS, PRIME_KEY, RFC, TORSION};

/// T1: each `MIXED_KEYS[i - 1]` is `PRIME_KEY` + `TORSION[i]` by this crate's arithmetic too,
/// and each is refused as of mixed order; (0, -1) is not the identity.
#[test]
fn the_keys_of_mixed_order() {
    let prime = Point::decode(&PRIME_KEY).expect("PRIME_KEY decodes");
    for (i, key) in MIXED_KEYS.iter().enumerate() {
        if cfg!(miri) && i > 0 {
            break;
        }
        let torsion = Point::decode(&TORSION[i + 1]).expect("a small-order point");
        assert_eq!(prime.add(&torsion).encode(), *key, "MIXED_KEYS[{i}] is PRIME_KEY + TORSION[{}]", i + 1);
    }
    if let Err(broke) = super::mixed_keys_refused(crate::Plant::None) {
        panic!("{broke}");
    }
}

/// T2.
#[test]
fn the_range_of_s_at_its_boundaries() {
    if let Err(broke) = super::s_bounds_hold(crate::Plant::None) {
        panic!("{broke}");
    }
}

/// T6.
#[test]
fn points_sharing_a_coordinate_differ() {
    if let Err(broke) = super::distinct_points_differ(crate::Plant::None) {
        panic!("{broke}");
    }
}

/// [s]B for any 32 bytes s (reduced mod L first, so S at or above L may be given).
fn times_base(s: &[u8; 32]) -> Point {
    let mut wide = [0u8; 64];
    wide[..32].copy_from_slice(s);
    double_scalar_mul_basepoint(&[0; 4], &Point::IDENTITY, &scalar::reduce_wide(&wide))
}

#[test]
fn the_rfc_vectors_verify() {
    for vector in &RFC {
        assert_eq!(verify(&vector.public, vector.message, &vector.signature), Ok(()), "{}", vector.name);
        // The message in two pieces, cut at every place: hashed in place, the same verdict.
        let key = VerifyingKey::decode(&vector.public).expect("the key");
        let cuts: Vec<usize> = if cfg!(miri) { vec![vector.message.len() / 2] } else { (0..=vector.message.len()).collect() };
        for cut in cuts {
            let (head, tail) = vector.message.split_at(cut);
            assert_eq!(key.verify_parts(&[head, tail], &vector.signature), Ok(()), "{} cut at {cut}", vector.name);
        }
        assert_eq!(verify_parts(&vector.public, &[vector.message], &vector.signature), Ok(()));
    }
}

#[test]
fn the_rfc_points_decode_and_encode_to_themselves() {
    for vector in &RFC {
        let r: [u8; 32] = vector.signature[..32].try_into().expect("32 bytes");
        for bytes in [vector.public, r] {
            let point = Point::decode(&bytes).expect("a canonical point");
            assert_eq!(point.encode(), bytes, "{}", vector.name);
            assert!(!point.is_small_order() && point.is_torsion_free(), "{}: of order L", vector.name);
        }
    }
}

/// Every single bit of every vector's key, message and signature, flipped one at a time, is
/// refused (under Miri, the low and high bit of the first and last byte of each part of TEST 1
/// and TEST 2).
#[test]
fn every_single_bit_flip_is_refused() {
    let vectors = if cfg!(miri) { &RFC[..2] } else { &RFC[..] };
    for vector in vectors {
        let len = vector.message.len();
        let total = 32 + len + 64;
        let bytes: Vec<usize> = if cfg!(miri) { vec![0, 31, 32, 32 + len, total - 1] } else { (0..total).collect() };
        for byte in bytes {
            if byte >= total || (byte >= 32 && byte < 32 + len && len == 0) {
                continue;
            }
            let bits: &[u32] = if cfg!(miri) { &[0, 7] } else { &[0, 1, 2, 3, 4, 5, 6, 7] };
            for &bit in bits {
                let mut key = vector.public;
                let mut message = vector.message.to_vec();
                let mut signature = vector.signature;
                if byte < 32 {
                    key[byte] ^= 1 << bit;
                } else if byte < 32 + len {
                    message[byte - 32] ^= 1 << bit;
                } else {
                    signature[byte - 32 - len] ^= 1 << bit;
                }
                assert!(verify(&key, &message, &signature).is_err(), "{}: byte {byte} bit {bit} flipped was accepted", vector.name);
            }
        }
    }
}

/// Each edge case's verdict, and the two equations its notes claim, recomputed here with the
/// crate's arithmetic (an independent check of the generator's notes).
#[test]
fn the_edge_cases() {
    for edge in &EDGE {
        assert_eq!(verify(&edge.key, edge.message, &edge.signature), edge.want, "{}: {}", edge.name, edge.why);
        let r_bytes: [u8; 32] = edge.signature[..32].try_into().expect("32 bytes");
        let s_bytes: [u8; 32] = edge.signature[32..].try_into().expect("32 bytes");
        let (Ok(a), Ok(r)) = (Point::decode(&edge.key), Point::decode(&r_bytes)) else {
            assert_eq!((edge.cofactorless, edge.cofactored), (None, None), "{}: a point does not decode", edge.name);
            continue;
        };
        let mut hasher = sha512::Sha512::new();
        hasher.update(&r_bytes);
        hasher.update(&edge.key);
        hasher.update(edge.message);
        let k = scalar::reduce_wide(&hasher.finalize());
        let left = times_base(&s_bytes);
        let right = r.add(&double_scalar_mul_basepoint(&k, &a, &[0; 4]));
        assert_eq!(Some(left.equals(&right)), edge.cofactorless, "{}: cofactorless", edge.name);
        assert_eq!(Some(left.sub(&right).mul_by_cofactor().is_identity()), edge.cofactored, "{}: cofactored", edge.name);
    }
}

#[test]
fn the_eight_points_of_small_order() {
    let t = Point::decode(&TORSION[1]).expect("an order-8 point");
    let mut multiple = Point::IDENTITY;
    let key = RFC[0].public;
    for (i, bytes) in TORSION.iter().enumerate() {
        let point = Point::decode(bytes).expect("a small-order point decodes");
        assert!(point.equals(&multiple), "TORSION[{i}] is [{i}]T");
        assert_eq!(point.encode(), *bytes);
        assert!(point.is_small_order(), "TORSION[{i}]");
        assert!(point.mul_by_cofactor().is_identity());
        assert_eq!(verify(bytes, b"", &RFC[0].signature), Err(Refused::KeySmallOrder), "TORSION[{i}] as a key");
        let mut signature = RFC[0].signature;
        signature[..32].copy_from_slice(bytes);
        assert_eq!(verify(&key, RFC[0].message, &signature), Err(Refused::RSmallOrder), "TORSION[{i}] as R");
        multiple = multiple.add(&t);
    }
    assert!(multiple.is_identity(), "[8]T is the identity");
}

#[test]
fn the_base_point_has_order_l() {
    assert_eq!(BASEPOINT.encode(), BASEPOINT_BYTES);
    assert!(!BASEPOINT.is_identity() && !BASEPOINT.is_small_order());
    assert!(BASEPOINT.is_torsion_free(), "[L]B is the identity");
    let mut l_minus_one = scalar::L;
    l_minus_one[0] -= 1;
    let point = double_scalar_mul_basepoint(&[0; 4], &Point::IDENTITY, &l_minus_one);
    assert!(point.equals(&BASEPOINT.neg()), "[L - 1]B is -B");
    // And by the other route: L - 1 multiples of B through the key's table.
    assert!(double_scalar_mul_basepoint(&l_minus_one, &BASEPOINT, &[0; 4]).equals(&BASEPOINT.neg()));
}

/// Every second encoding of a y: p + y for each y below 19, both sign bits, and the largest
/// 255-bit value; and the sign-bit encodings of the two points with x = 0.
#[test]
fn every_noncanonical_encoding_is_refused() {
    let p: [u8; 32] = data::hex("edffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f");
    for y in 0u8..19 {
        for sign in [0u8, 0x80] {
            let mut bytes = p;
            bytes[0] += y;
            bytes[31] |= sign;
            assert_eq!(Point::decode(&bytes).map(|_| ()), Err(Encoding::YNotReduced), "p + {y}, sign {sign:#x}");
        }
    }
    assert_eq!(Point::decode(&[0xff; 32]).map(|_| ()), Err(Encoding::YNotReduced));
    for x_zero in [TORSION[0], TORSION[4]] {
        let mut bytes = x_zero;
        bytes[31] |= 0x80;
        assert_eq!(Point::decode(&bytes).map(|_| ()), Err(Encoding::NegativeZero));
    }
}
