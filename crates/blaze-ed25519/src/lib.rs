//! Ed25519 signature verification, written here (RFC 8032, section 5.1.7), with the strict
//! choices made explicit: the check the shared cache's reader makes of a namespace's signed head
//! before it believes a fetched byte (docs/v2/plan/design/remote-read.md, "Ed25519, written
//! here"; the seam is `HeadCheck` in crates/blaze-remote/src/read/form.rs).
//!
//! # Verification only
//!
//! There is no signing and no key generation here, on purpose. Verification computes only on
//! public values (the key, the message, the signature), so it needs no secret-independent
//! timing, no zeroing of memory and no defence against a side channel: variable-time code
//! (signed-digit windows, early exits, branches on the data) is sound, and that is what makes it
//! small enough to own. Signing handles a secret and needs all of those; the writer of the cache
//! is CI, which signs with its own tooling, so blaze never signs. Nor is there batch
//! verification: a reader checks one head per namespace per command.
//!
//! # What is checked, in order
//!
//! [`VerifyingKey::decode`], once per key (rule 74: the key is a proof type later checks take):
//! 1. the key's 32 bytes are a canonical point encoding: y below p, and not x = 0 with the sign
//!    bit set (else [`Refused::KeyEncoding`]), and a point of the curve;
//! 2. the key is not of small order, [8]A != identity ([`Refused::KeySmallOrder`]);
//! 3. the key has order exactly L, [L]A = identity ([`Refused::KeyMixedOrder`]): no honest key
//!    generation makes any other, and with it the cofactorless and cofactored equations can only
//!    disagree on a mixed-order R.
//!
//! [`VerifyingKey::verify_parts`], per signature:
//! 4. S, the signature's second half, is below the group order L ([`Refused::SNotBelowOrder`]):
//!    else S + L would be a second valid signature (malleability);
//! 5. R, the first half, is a canonical point encoding ([`Refused::REncoding`]);
//! 6. R is not of small order ([`Refused::RSmallOrder`]);
//! 7. k = SHA-512(R || A || message) mod L, and the cofactorless equation [S]B = R + [k]A holds,
//!    compared as points ([`Refused::Mismatch`]).
//!
//! # The small-order and cofactor decisions
//!
//! The reader's question is "is this statement signed by the one key I was given". The
//! strictest widely specified rule answers it with the fewest accepted forms: the cofactorless
//! equation, which RFC 8032 implementations commonly use (libsodium, BoringSSL, `ring`, Go), so
//! every signature CI's tooling makes is accepted, and every signature that only a cofactored
//! verifier (ZIP-215) would take is refused; small-order keys and R refused as libsodium does;
//! and keys of mixed order refused besides, which only costs time once per key. The 12 cases of
//! Chalkias, Garillot and Nikolaenko, "Taming the many EdDSAs" (2020), and Project Wycheproof's
//! 210 Ed25519 vectors are test data in `tests/data/` (its README names their sources), each
//! checked for its verdict and its reason by `tests/published.rs`.
//!
//! # The public surface
//!
//! [`verify`], [`verify_parts`], [`VerifyingKey`], [`Refused`] and [`Encoding`]: every function
//! is total, its contract in its signature (a key of 32 bytes, a signature of 64). And
//! [`sha512`], SHA-512 on any bytes. The field, the scalars and the points are the crate's own
//! (`field.rs`, `scalar.rs`, `point.rs`): their functions have preconditions (a scalar below
//! 2^255, limbs below a bound) that only this crate's callers are held to; `cost`, hidden from
//! the docs, gives `examples/cost.rs` the few it times, each wrapped total.
//!
//! # Representation and cost
//!
//! The field is five 51-bit limbs with 128-bit products (`field.rs`); the double-scalar
//! multiplication is Straus's method with width-5 signed digits for the key and width-8 ones for
//! the base point, whose 64 odd multiples the compiler computes from its encoding (`point.rs`).
//! Measured and set against its floor in `examples/cost.rs`.
//!
//! `no_std`, no allocation, no recursion, no unsafe code, two dependencies (blaze-int, and
//! blaze-own for the plant seam: each check a control can weaken takes `plant: impl
//! Planted<Plant>`, and the public functions pass `NoPlant`).

#![no_std]
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core, blaze::std_use, blaze::implicit_allocator, blaze::process_static)]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

mod field;
mod point;
mod scalar;
pub mod sha512;

#[cfg(test)]
mod tests;

use core::fmt;

use blaze_own::planted::{NoPlant, Planted, Plants};

pub use point::Encoding;
use point::Point;

/// Which check refused a signature: every reason names the step of the crate's doc it failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Refused {
    /// Step 1: the public key is not a canonical encoding of a curve point.
    KeyEncoding(Encoding),
    /// Step 2: the public key is one of the eight points of small order.
    KeySmallOrder,
    /// Step 3: the public key has a small-order component: its order is not L.
    KeyMixedOrder,
    /// Step 4: S is L or above.
    SNotBelowOrder,
    /// Step 5: R is not a canonical encoding of a curve point.
    REncoding(Encoding),
    /// Step 6: R is one of the eight points of small order.
    RSmallOrder,
    /// Step 7: [S]B is not R + [k]A: the signature is not this key's over this message.
    Mismatch,
}

impl fmt::Display for Encoding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Encoding::YNotReduced => "its y coordinate is p or above (a second encoding of a point)",
            Encoding::NotOnCurve => "no point of the curve has its y coordinate",
            Encoding::NegativeZero => "its x is 0 and its sign bit is set (a second encoding of a point)",
        })
    }
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refused::KeyEncoding(why) => write!(f, "Ed25519: the public key is refused: {why}"),
            Refused::KeySmallOrder => f.write_str("Ed25519: the public key is a point of small order"),
            Refused::KeyMixedOrder => f.write_str("Ed25519: the public key has a small-order component (its order is not L)"),
            Refused::SNotBelowOrder => f.write_str("Ed25519: the signature's S is not below the group order L (a malleated signature)"),
            Refused::REncoding(why) => write!(f, "Ed25519: the signature's R is refused: {why}"),
            Refused::RSmallOrder => f.write_str("Ed25519: the signature's R is a point of small order"),
            Refused::Mismatch => f.write_str("Ed25519: the signature is not the key's over this message ([S]B != R + [k]A)"),
        }
    }
}

/// A public key that passed steps 1 to 3: the proof later verifications take, so a key given
/// once is checked once.
#[derive(Clone, Copy, Debug)]
pub struct VerifyingKey {
    bytes: [u8; 32],
    /// -A: the multiplication computes [S]B + [k](-A).
    minus_a: Point,
}

impl VerifyingKey {
    /// Steps 1 to 3 of the crate's doc.
    pub fn decode(bytes: &[u8; 32]) -> Result<VerifyingKey, Refused> {
        VerifyingKey::decode_planted(bytes, NoPlant)
    }

    /// [`VerifyingKey::decode`] with its plant seam: [`NoPlant`] where it ships.
    pub(crate) fn decode_planted(bytes: &[u8; 32], plant: impl Planted<Plant>) -> Result<VerifyingKey, Refused> {
        let a = Point::decode(bytes).map_err(Refused::KeyEncoding)?;
        if a.is_small_order_planted(plant) {
            return Err(Refused::KeySmallOrder);
        }
        if !a.is_torsion_free_planted(plant) {
            return Err(Refused::KeyMixedOrder);
        }
        Ok(VerifyingKey { bytes: *bytes, minus_a: a.neg() })
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.bytes
    }

    /// Steps 4 to 7 for `message`.
    pub fn verify(&self, message: &[u8], signature: &[u8; 64]) -> Result<(), Refused> {
        self.verify_parts(&[message], signature)
    }

    /// Steps 4 to 7 for the message that is `parts` joined, hashed in place: a caller that signs
    /// `domain || 0 || body` passes the three pieces and copies nothing.
    pub fn verify_parts(&self, parts: &[&[u8]], signature: &[u8; 64]) -> Result<(), Refused> {
        self.verify_parts_planted(parts, signature, NoPlant)
    }

    /// [`VerifyingKey::verify_parts`] with its plant seam: [`NoPlant`] where it ships.
    pub(crate) fn verify_parts_planted(
        &self,
        parts: &[&[u8]],
        signature: &[u8; 64],
        plant: impl Planted<Plant>,
    ) -> Result<(), Refused> {
        let (r_bytes, s_bytes) = signature.split_at(32);
        let r_bytes: &[u8; 32] = r_bytes.try_into().expect("a signature's first half is 32 bytes");
        let s_bytes: &[u8; 32] = s_bytes.try_into().expect("a signature's second half is 32 bytes");
        if !scalar::is_canonical_planted(s_bytes, plant) && !plant.has(Plant::SRangeUnchecked) {
            return Err(Refused::SNotBelowOrder);
        }
        let r = decode_r(r_bytes, plant).map_err(Refused::REncoding)?;
        if r.is_small_order_planted(plant) {
            return Err(Refused::RSmallOrder);
        }
        let mut hasher = sha512::Sha512::new();
        hasher.update(r_bytes);
        hasher.update(&self.bytes);
        for part in parts {
            hasher.update(part);
        }
        let k = scalar::reduce_wide(&hasher.finalize());
        let s = scalar::from_bytes(s_bytes);
        let r_check = point::double_scalar_mul_basepoint(&k, &self.minus_a, &s);
        if same_point(&r_check, &r, plant) { Ok(()) } else { Err(Refused::Mismatch) }
    }
}

/// Steps 1 to 7: `public_key` checked, then `signature` over `message`.
pub fn verify(public_key: &[u8; 32], message: &[u8], signature: &[u8; 64]) -> Result<(), Refused> {
    VerifyingKey::decode(public_key)?.verify(message, signature)
}

/// [`verify`] of the message that is `parts` joined, hashed in place.
pub fn verify_parts(public_key: &[u8; 32], parts: &[&[u8]], signature: &[u8; 64]) -> Result<(), Refused> {
    VerifyingKey::decode(public_key)?.verify_parts(parts, signature)
}

/// R's point: canonical decoding (step 5).
fn decode_r(bytes: &[u8; 32], plant: impl Planted<Plant>) -> Result<Point, Encoding> {
    if plant.has(Plant::RNonCanonical) {
        return Point::decode_y(field::Fe::from_bytes(bytes), bytes[31] >> 7 == 1);
    }
    Point::decode(bytes)
}

/// The comparison of step 7, as points: cross-multiplied by the other's Z, then compared by
/// canonical bytes. Since R was decoded canonically, equal points means R's bytes are the
/// encoding of [S]B - [k]A. A computed Z of zero is no point at all ((0 : 0 : 0) would equal
/// every point by the cross-multiplication): no correct arithmetic makes one, so one is a bug,
/// and it is refused rather than let it accept everything.
pub(crate) fn same_point(computed: &Point, r: &Point, plant: impl Planted<Plant>) -> bool {
    if plant.has(Plant::CompareUnnormalised) {
        return computed.unnormalised() == r.unnormalised();
    }
    !computed.z_is_zero() && computed.equals_planted(r, plant)
}

/// A bug a control plants, to show the tests catch it (D165, rule 78): taken through the
/// `Planted<Plant>` seam, which shipped code fills with [`NoPlant`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[cfg_attr(not(test), expect(dead_code, reason = "shipped code passes NoPlant; only the controls make a plant"))]
pub(crate) enum Plant {
    /// The code as it is.
    None,
    /// Step 4 skipped: S at or above L taken.
    SRangeUnchecked,
    /// Step 5 lenient: R's y read modulo p, so a second encoding of a point is taken.
    RNonCanonical,
    /// Step 7 compares the computed point's X and Y as held, not divided by Z.
    CompareUnnormalised,
    /// The identity test reads X = 0 alone, so (0, -1) counts as the identity and a key of
    /// order 2L passes [L]A = O.
    IdentityByXOnly,
    /// The torsion check is [4L]A = O, so keys of order 2L and 4L pass.
    TorsionBy4L,
    /// Point equality compares X/Z alone, so P and (x, -y) are one point.
    EqualsByXOnly,
    /// S's range check is S <= L, so S = L is taken.
    SAtOrderTaken,
    /// S's range check compares the top limb alone, so every S in [L, 2^252 + 2^192) is taken.
    STopLimbOnly,
}

impl Plants for Plant {}

/// Not an interface: the few internals `examples/cost.rs` times and counts, each wrapped so it
/// is total (no precondition a caller could break). Nothing but that example uses it.
#[doc(hidden)]
pub mod cost {
    use crate::field::Fe;
    use crate::{point, scalar};

    /// A field element as `Fe::from_bytes`, `mul` and `square` leave it, limbs below 2^52: any
    /// of them is a valid input to `mul` and `square`, so no chain of these breaks a bound.
    #[derive(Clone, Copy, Debug)]
    pub struct Element(Fe);

    impl Element {
        pub const fn from_bytes(bytes: &[u8; 32]) -> Element {
            Element(Fe::from_bytes(bytes))
        }

        pub const fn mul(&self, other: &Element) -> Element {
            Element(self.0.mul(&other.0))
        }

        pub const fn square(&self) -> Element {
            Element(self.0.square())
        }
    }

    /// The signed digits a verification of `signature` over `message` under `public_key`
    /// walks: k = SHA-512(R || A || M) mod L at the key's width, and S (taken mod L, so any 32
    /// bytes may be given) at the base point's.
    pub fn digits(public_key: &[u8; 32], message: &[u8], signature: &[u8; 64]) -> ([i8; 256], [i8; 256]) {
        let mut hasher = crate::sha512::Sha512::new();
        hasher.update(&signature[..32]);
        hasher.update(public_key);
        hasher.update(message);
        let k = scalar::reduce_wide(&hasher.finalize());
        let mut s_wide = [0u8; 64];
        s_wide[..32].copy_from_slice(&signature[32..]);
        let s = scalar::reduce_wide(&s_wide);
        (scalar::naf(&k, point::POINT_WIDTH), scalar::naf(&s, point::BASE_WIDTH))
    }

    /// The signed digits of L at the key's width: the key check's [L]A.
    pub fn order_digits() -> [i8; 256] {
        scalar::naf(&scalar::L, point::POINT_WIDTH)
    }
}
