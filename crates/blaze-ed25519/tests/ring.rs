//! A differential test against a second Ed25519, `ring` (test-only, never in the normal graph):
//! `ring` signs a drawn message under a key from a drawn seed, this crate must accept it, and
//! then a drawn mutation of the key, message or signature must get the same verdict from both.
//! `ring` verifies by the same cofactorless equation and also refuses S at or above L; it does
//! not refuse small-order or mixed-order points, which no drawn mutation here makes valid but
//! with a chance of about 2^-250. Not under Miri: `ring` is C and assembly.

#![cfg(not(miri))]

use blaze_ed25519::verify;
use blaze_sim::{Config, Tape};
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};

/// L, little-endian.
const L: [u8; 32] = [
    0xed, 0xd3, 0xf5, 0x5c, 0x1a, 0x63, 0x12, 0x58, 0xd6, 0x9c, 0xf7, 0xa2, 0xde, 0xf9, 0xde, 0x14, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0x10,
];

fn ring_accepts(key: &[u8; 32], message: &[u8], signature: &[u8; 64]) -> bool {
    UnparsedPublicKey::new(&ED25519, key).verify(message, signature).is_ok()
}

fn mutate(tape: &Tape, key: &mut [u8; 32], message: &mut Vec<u8>, signature: &mut [u8; 64]) -> &'static str {
    match tape.below(6, "mutation") {
        0 => "none",
        1 => {
            let bit = tape.index(256, "key bit");
            key[bit / 8] ^= 1 << (bit % 8);
            "a key bit"
        }
        2 => {
            let bit = tape.index(512, "signature bit");
            signature[bit / 8] ^= 1 << (bit % 8);
            "a signature bit"
        }
        3 => {
            if message.is_empty() {
                message.push(0);
            } else {
                let bit = tape.index(message.len() * 8, "message bit");
                message[bit / 8] ^= 1 << (bit % 8);
            }
            "a message bit"
        }
        4 => {
            // S + L: the same point equation, a second form.
            let mut carry = 0u16;
            for (s, l) in signature[32..].iter_mut().zip(L) {
                let sum = u16::from(*s) + u16::from(l) + carry;
                *s = sum as u8;
                carry = sum >> 8;
            }
            "S + L"
        }
        _ => {
            tape.fill(&mut signature[..], "signature");
            "a drawn signature"
        }
    }
}

#[test]
fn verdicts_equal_rings() {
    blaze_sim::check(Config::cases(2000), |tape| {
        let mut seed = [0u8; 32];
        tape.fill(&mut seed, "seed");
        let pair = Ed25519KeyPair::from_seed_unchecked(&seed).expect("any 32 bytes are a seed");
        let mut key: [u8; 32] = pair.public_key().as_ref().try_into().expect("32 bytes");
        let mut message = vec![0u8; tape.index(300, "message length")];
        tape.fill(&mut message, "message");
        let mut signature: [u8; 64] = pair.sign(&message).as_ref().try_into().expect("64 bytes");
        assert_eq!(verify(&key, &message, &signature), Ok(()), "ring's own signature");
        let what = mutate(tape, &mut key, &mut message, &mut signature);
        let ours = verify(&key, &message, &signature);
        assert_eq!(ours.is_ok(), ring_accepts(&key, &message, &signature), "after {what}: this crate says {ours:?}");
        if what != "none" {
            assert!(ours.is_err(), "after {what}: accepted");
        }
    });
}
