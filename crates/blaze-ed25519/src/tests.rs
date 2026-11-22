//! Planted-bug controls (rule 43, `blaze_sim::control`): the vector tests are trusted only once
//! they have been seen to catch each bug planted on purpose, and to pass the code as it is.
//! Every case checks every RFC 8032 vector and every edge case of `tests/data/mod.rs` for its
//! exact verdict, the keys of mixed order, the range check of S at its boundaries and point
//! equality on points that share one coordinate, then one drawn single-bit flip of an RFC
//! vector's key, message or signature, which must be refused.
//!
//! The internals' own tests are this module's children: `algebra` (the field, the scalars, the
//! group), `vectors` (verification on the typed vectors, the points of small order). The
//! published vectors are an integration test on the public surface (`tests/published.rs`).

mod algebra;
mod vectors;

use std::format;

use blaze_sim::{Broke, Config, Tape};

use blaze_own::planted::NoPlant;

use crate::point::{BASEPOINT, Point};
use crate::{Encoding, Plant, Refused, VerifyingKey, same_point, scalar, verify};

/// [`verify`] with `plant` planted.
fn verify_planted(public_key: &[u8; 32], message: &[u8], signature: &[u8; 64], plant: Plant) -> Result<(), Refused> {
    VerifyingKey::decode_planted(public_key, plant)?.verify_parts_planted(&[message], signature, plant)
}

#[path = "../tests/data/mod.rs"]
mod data;

/// Cases a control runs: under Miri, which interprets every field multiplication, one.
fn config() -> Config {
    if cfg!(miri) { Config::cases(1) } else { Config::cases(12) }
}

/// The edge cases checked: under Miri, only the three that catch the plants and their control.
fn edges() -> impl Iterator<Item = &'static data::Edge> {
    data::EDGE.iter().filter(|edge| {
        !cfg!(miri) || matches!(edge.name, "valid" | "s_plus_order" | "noncanonical_identity_r")
    })
}

fn judge(name: &str, want: Result<(), Refused>, got: Result<(), Refused>) -> Result<(), Broke> {
    match (want, got) {
        (want, got) if want == got => Ok(()),
        (Ok(()), Err(why)) => Err(Broke::Oracle(format!("{name}: a valid signature refused ({why:?})"))),
        (Err(want), Ok(())) => Err(Broke::Oracle(format!("{name}: a hostile signature accepted (want {want:?})"))),
        (want, got) => Err(Broke::Oracle(format!("{name}: refused for the wrong reason: want {want:?}, got {got:?}"))),
    }
}

/// T1: `PRIME_KEY` + `TORSION[i]` is refused as `KeyMixedOrder` for every i in 1..8 (under
/// Miri, the keys of order 4L and 2L, the two a weakened check lets through), and (0, -1) is
/// not the identity.
pub(crate) fn mixed_keys_refused(plant: Plant) -> Result<(), Broke> {
    for (i, key) in data::MIXED_KEYS.iter().enumerate() {
        if cfg!(miri) && !matches!(i + 1, 2 | 4) {
            continue;
        }
        match VerifyingKey::decode_planted(key, plant).map(|_| ()) {
            Err(Refused::KeyMixedOrder) => {}
            Ok(()) => return Err(Broke::Oracle(format!("PRIME_KEY + TORSION[{}]: a key of mixed order accepted", i + 1))),
            Err(other) => {
                return Err(Broke::Oracle(format!("PRIME_KEY + TORSION[{}]: refused for the wrong reason: {other:?}", i + 1)));
            }
        }
    }
    let minus_one = Point::decode(&data::TORSION[4]).map_err(|why| Broke::Oracle(format!("(0, -1) does not decode: {why:?}")))?;
    if minus_one.is_identity_planted(plant) {
        return Err(Broke::Oracle("(0, -1) taken for the identity".into()));
    }
    Ok(())
}

/// T2: S's range check at its boundaries: L - 1 and L - 2^192 below L; L, L + 1, L + 2^64,
/// L + 2^128 and L + 2^192 not.
pub(crate) fn s_bounds_hold(plant: Plant) -> Result<(), Broke> {
    let l = scalar::L;
    let cases: [(&str, [u64; 4], bool); 7] = [
        ("L - 1", [l[0] - 1, l[1], l[2], l[3]], true),
        ("L - 2^192", [l[0], l[1], l[2], l[3] - 1], true),
        ("L", l, false),
        ("L + 1", [l[0] + 1, l[1], l[2], l[3]], false),
        ("L + 2^64", [l[0], l[1] + 1, l[2], l[3]], false),
        ("L + 2^128", [l[0], l[1], l[2] + 1, l[3]], false),
        ("L + 2^192", [l[0], l[1], l[2], l[3] + 1], false),
    ];
    for (name, limbs, want) in cases {
        let got = scalar::is_canonical_planted(&scalar::to_bytes(&limbs), plant);
        if got != want {
            return Err(Broke::Oracle(format!("is_canonical({name}) is {got}, want {want}")));
        }
    }
    Ok(())
}

/// T6: two points that share one coordinate are not equal: B and -B (same y), B and
/// (0, -1) - B = (x, -y) (same x).
pub(crate) fn distinct_points_differ(plant: Plant) -> Result<(), Broke> {
    let minus_one = Point::decode(&data::TORSION[4]).map_err(|why| Broke::Oracle(format!("(0, -1) does not decode: {why:?}")))?;
    if BASEPOINT.equals_planted(&BASEPOINT.neg(), plant) {
        return Err(Broke::Oracle("two different points compared equal: B and -B".into()));
    }
    if BASEPOINT.equals_planted(&minus_one.sub(&BASEPOINT), plant) {
        return Err(Broke::Oracle("two different points compared equal: B and (0, -1) - B".into()));
    }
    Ok(())
}

fn run(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let rfc = if cfg!(miri) { &data::RFC[..1] } else { &data::RFC[..] };
    for vector in rfc {
        judge(vector.name, Ok(()), verify_planted(&vector.public, vector.message, &vector.signature, plant))?;
    }
    for edge in edges() {
        judge(edge.name, edge.want, verify_planted(&edge.key, edge.message, &edge.signature, plant))?;
    }
    mixed_keys_refused(plant)?;
    s_bounds_hold(plant)?;
    distinct_points_differ(plant)?;
    let vector = &rfc[tape.index(rfc.len(), "vector")];
    let mut key = vector.public;
    let mut message = [0u8; 1024];
    let len = vector.message.len();
    message[..len].copy_from_slice(vector.message);
    let mut signature = vector.signature;
    let bit = tape.index((32 + len + 64) * 8, "bit to flip");
    let (byte, mask) = (bit / 8, 1u8 << (bit % 8));
    if byte < 32 {
        key[byte] ^= mask;
    } else if byte < 32 + len {
        message[byte - 32] ^= mask;
    } else {
        signature[byte - 32 - len] ^= mask;
    }
    match verify_planted(&key, &message[..len], &signature, plant) {
        Ok(()) => Err(Broke::Oracle(format!("{}: a hostile signature accepted (bit {bit} flipped)", vector.name))),
        Err(_) => Ok(()),
    }
}

#[test]
fn the_vectors_catch_a_missing_range_check_of_s() {
    blaze_sim::control(config(), Plant::None, Plant::SRangeUnchecked, "hostile signature accepted", run);
}

#[test]
fn the_vectors_catch_a_noncanonical_r_decoded() {
    blaze_sim::control(config(), Plant::None, Plant::RNonCanonical, "refused for the wrong reason", run);
}

#[test]
fn the_vectors_catch_a_comparison_not_normalised() {
    blaze_sim::control(config(), Plant::None, Plant::CompareUnnormalised, "valid signature refused", run);
}

/// The review's mutant M1: the key of order 2L passes [L]A = O when (0, -1) counts.
#[test]
fn the_keys_catch_an_identity_test_of_x_alone() {
    blaze_sim::control(config(), Plant::None, Plant::IdentityByXOnly, "a key of mixed order accepted", run);
}

/// The review's mutant M20: keys of order 2L and 4L pass [4L]A = O.
#[test]
fn the_keys_catch_a_torsion_check_of_4l() {
    blaze_sim::control(config(), Plant::None, Plant::TorsionBy4L, "a key of mixed order accepted", run);
}

/// The review's mutant M7.
#[test]
fn the_points_catch_an_equality_of_x_alone() {
    blaze_sim::control(config(), Plant::None, Plant::EqualsByXOnly, "two different points compared equal", run);
}

/// The review's mutant M2.
#[test]
fn the_bounds_catch_s_equal_to_l_taken() {
    blaze_sim::control(config(), Plant::None, Plant::SAtOrderTaken, "is_canonical(L) is true", run);
}

/// The review's mutant M3.
#[test]
fn the_bounds_catch_a_range_check_of_the_top_limb() {
    blaze_sim::control(config(), Plant::None, Plant::STopLimbOnly, "is_canonical(L) is true", run);
}

/// The plant of a lenient R decoding is caught by the refusal's reason, not by an acceptance:
/// no non-canonical R can complete a valid signature without its discrete log, and the only
/// points with a second encoding of y (y below 19) whose logs are known are of small order,
/// which step 6 refuses. The test asks for the exact reason so the lenient decoding still shows.
#[test]
fn a_lenient_r_decoding_is_refused_only_for_small_order() {
    let edge = data::EDGE.iter().find(|edge| edge.name == "noncanonical_identity_r").expect("the case");
    let got = verify_planted(&edge.key, edge.message, &edge.signature, Plant::RNonCanonical);
    assert_eq!(got, Err(Refused::RSmallOrder));
    assert_eq!(verify(&edge.key, edge.message, &edge.signature), Err(Refused::REncoding(Encoding::YNotReduced)));
}

/// C2: (0 : 0 : 0) passes the identity test and the cross-multiplied equality with every point
/// (which is why step 7 must not take it), and step 7's comparison refuses it.
#[test]
fn a_degenerate_point_is_never_the_signature_s() {
    let degenerate = Point::DEGENERATE;
    assert!(degenerate.is_identity() && degenerate.equals(&BASEPOINT), "the cross-multiplication alone takes it");
    for r in [BASEPOINT, Point::IDENTITY, BASEPOINT.neg()] {
        assert!(!same_point(&degenerate, &r, NoPlant), "step 7 refuses a computed (0 : 0 : 0)");
        assert!(same_point(&r, &r, NoPlant), "and takes a point as itself");
    }
}
